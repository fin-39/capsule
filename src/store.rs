use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use fs2::FileExt;
use thiserror::Error;
use uuid::Uuid;

use crate::model::{CapsuleRecord, LIBRARY_FORMAT_VERSION, LibraryState, ModelError};

/// An atomic, process-safe JSON store for the capsule library.
#[derive(Debug, Clone)]
pub struct LibraryStore {
    path: PathBuf,
}

impl LibraryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load an existing library. A missing library is reported distinctly.
    pub fn load(&self) -> Result<LibraryState, StoreError> {
        if !self.path.exists() {
            return Err(StoreError::Missing(self.path.clone()));
        }

        let lock = self.open_lock_file()?;
        FileExt::lock_shared(&lock).map_err(|source| self.io_error(self.lock_path(), source))?;
        self.load_unlocked()
    }

    /// Load a library, or return a new in-memory library when it does not exist.
    /// Corrupt or unsupported files are never silently replaced.
    pub fn load_or_default(&self) -> Result<LibraryState, StoreError> {
        match self.load() {
            Err(StoreError::Missing(_)) => Ok(LibraryState::default()),
            result => result,
        }
    }

    /// Atomically replace the on-disk library.
    pub fn save(&self, state: &LibraryState) -> Result<(), StoreError> {
        self.ensure_parent()?;
        let lock = self.open_lock_file()?;
        FileExt::lock_exclusive(&lock).map_err(|source| self.io_error(self.lock_path(), source))?;
        self.save_unlocked(state)
    }

    pub fn list(&self) -> Result<Vec<CapsuleRecord>, StoreError> {
        Ok(self.load()?.capsules)
    }

    pub fn get(&self, id: Uuid) -> Result<CapsuleRecord, StoreError> {
        self.load()?
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound(id))
    }

    /// Add and persist a record. A fresh UUID is assigned if the supplied UUID
    /// is nil or already exists, so records created by importers cannot collide.
    pub fn create(&self, mut capsule: CapsuleRecord) -> Result<CapsuleRecord, StoreError> {
        self.mutate(true, |library| {
            if let Some(path) = capsule.storage.image_path()
                && library
                    .capsules
                    .iter()
                    .filter_map(|existing| existing.storage.image_path())
                    .any(|existing| existing == path)
            {
                return Err(StoreError::DuplicateImage(path.to_path_buf()));
            }
            while capsule.id.is_nil() || library.get(capsule.id).is_some() {
                capsule.id = Uuid::new_v4();
            }
            library.insert(capsule.clone())?;
            Ok(capsule)
        })
    }

    pub fn update(&self, capsule: CapsuleRecord) -> Result<(), StoreError> {
        self.mutate(false, |library| {
            library.replace(capsule).map_err(map_crud_error)
        })
    }

    /// Remove only the library record. Deleting capsule storage is deliberately
    /// a separate, explicitly destructive operation owned by the frontend.
    pub fn delete(&self, id: Uuid) -> Result<CapsuleRecord, StoreError> {
        self.mutate(false, |library| library.remove(id).map_err(map_crud_error))
    }

    fn mutate<T>(
        &self,
        create_if_missing: bool,
        operation: impl FnOnce(&mut LibraryState) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.ensure_parent()?;
        let lock = self.open_lock_file()?;
        FileExt::lock_exclusive(&lock).map_err(|source| self.io_error(self.lock_path(), source))?;

        let mut state = if self.path.exists() {
            self.load_unlocked()?
        } else if create_if_missing {
            LibraryState::default()
        } else {
            return Err(StoreError::Missing(self.path.clone()));
        };

        let result = operation(&mut state)?;
        self.save_unlocked(&state)?;
        Ok(result)
    }

    fn load_unlocked(&self) -> Result<LibraryState, StoreError> {
        let bytes = fs::read(&self.path).map_err(|source| self.io_error(&self.path, source))?;
        let state: LibraryState =
            serde_json::from_slice(&bytes).map_err(|source| StoreError::Corrupt {
                path: self.path.clone(),
                source,
            })?;

        if state.version != LIBRARY_FORMAT_VERSION {
            return Err(StoreError::UnsupportedVersion {
                path: self.path.clone(),
                found: state.version,
                supported: LIBRARY_FORMAT_VERSION,
            });
        }
        state.validate()?;
        Ok(state)
    }

    fn save_unlocked(&self, state: &LibraryState) -> Result<(), StoreError> {
        if state.version != LIBRARY_FORMAT_VERSION {
            return Err(StoreError::UnsupportedVersion {
                path: self.path.clone(),
                found: state.version,
                supported: LIBRARY_FORMAT_VERSION,
            });
        }
        state.validate()?;

        let mut bytes = serde_json::to_vec_pretty(state).map_err(StoreError::Encode)?;
        bytes.push(b'\n');

        let temporary_path = self.temporary_path();
        let write_result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut temporary = options
                .open(&temporary_path)
                .map_err(|source| self.io_error(&temporary_path, source))?;
            temporary
                .write_all(&bytes)
                .map_err(|source| self.io_error(&temporary_path, source))?;
            temporary
                .sync_all()
                .map_err(|source| self.io_error(&temporary_path, source))?;
            fs::rename(&temporary_path, &self.path)
                .map_err(|source| self.io_error(&self.path, source))?;
            self.sync_parent()
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        write_result
    }

    fn ensure_parent(&self) -> Result<(), StoreError> {
        let parent = self.parent_dir();
        fs::create_dir_all(parent).map_err(|source| self.io_error(parent, source))
    }

    fn sync_parent(&self) -> Result<(), StoreError> {
        let parent = self.parent_dir();
        let directory = File::open(parent).map_err(|source| self.io_error(parent, source))?;
        directory
            .sync_all()
            .map_err(|source| self.io_error(parent, source))
    }

    fn open_lock_file(&self) -> Result<File, StoreError> {
        let path = self.lock_path();
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        options
            .open(&path)
            .map_err(|source| self.io_error(path, source))
    }

    fn parent_dir(&self) -> &Path {
        self.path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = OsString::from(".");
        name.push(
            self.path
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("capsule-library")),
        );
        name.push(".lock");
        self.parent_dir().join(name)
    }

    fn temporary_path(&self) -> PathBuf {
        let mut name = OsString::from(".");
        name.push(
            self.path
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("capsule-library")),
        );
        name.push(format!(".{}.tmp", Uuid::new_v4()));
        self.parent_dir().join(name)
    }

    fn io_error(&self, path: impl Into<PathBuf>, source: io::Error) -> StoreError {
        StoreError::Io {
            path: path.into(),
            source,
        }
    }
}

fn map_crud_error(error: ModelError) -> StoreError {
    match error {
        ModelError::NotFound(id) => StoreError::NotFound(id),
        error => StoreError::InvalidModel(error),
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("capsule library does not exist: {0}")]
    Missing(PathBuf),
    #[error("failed to access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("capsule library is not valid JSON ({path}): {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot encode capsule library: {0}")]
    Encode(#[source] serde_json::Error),
    #[error(
        "unsupported capsule library version {found} in {path}; this build supports version {supported}"
    )]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("capsule was not found: {0}")]
    NotFound(Uuid),
    #[error("capsule image is already registered: {0}")]
    DuplicateImage(PathBuf),
    #[error(transparent)]
    InvalidModel(#[from] ModelError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RunnerKind, StorageKind};
    use tempfile::tempdir;

    fn record(root: &Path, name: &str) -> CapsuleRecord {
        CapsuleRecord::new(
            name,
            StorageKind::Image {
                path: root.join(format!("{name}.capsule")),
            },
            "drive_c/game.exe",
            RunnerKind::Wine,
        )
    }

    #[test]
    fn missing_and_corrupt_libraries_have_distinct_errors() {
        let directory = tempdir().unwrap();
        let store = LibraryStore::new(directory.path().join("library.json"));
        assert!(matches!(store.load(), Err(StoreError::Missing(_))));

        fs::write(store.path(), b"not json").unwrap();
        assert!(matches!(store.load(), Err(StoreError::Corrupt { .. })));
    }

    #[test]
    fn save_load_and_crud_round_trip() {
        let directory = tempdir().unwrap();
        let store = LibraryStore::new(directory.path().join("state/library.json"));

        let created = store
            .create(record(directory.path(), "Legacy Game"))
            .unwrap();
        assert_eq!(store.get(created.id).unwrap(), created);

        let mut updated = created.clone();
        updated.name = "Legacy Game Updated".into();
        store.update(updated.clone()).unwrap();
        assert_eq!(store.list().unwrap(), vec![updated.clone()]);

        assert_eq!(store.delete(updated.id).unwrap(), updated);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn create_replaces_a_colliding_uuid() {
        let directory = tempdir().unwrap();
        let store = LibraryStore::new(directory.path().join("library.json"));
        let first = store.create(record(directory.path(), "One")).unwrap();
        let mut second = record(directory.path(), "Two");
        second.id = first.id;

        let second = store.create(second).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn create_rejects_an_image_already_registered_under_another_name() {
        let directory = tempdir().unwrap();
        let store = LibraryStore::new(directory.path().join("library.json"));
        let first = store.create(record(directory.path(), "One")).unwrap();
        let duplicate = CapsuleRecord::new(
            "Another name",
            StorageKind::ExternalImage {
                path: first.storage.path().to_path_buf(),
            },
            "drive_c/game.exe",
            RunnerKind::Wine,
        );

        assert!(matches!(
            store.create(duplicate),
            Err(StoreError::DuplicateImage(_))
        ));
        assert_eq!(store.list().unwrap(), vec![first]);
    }

    #[test]
    fn unsupported_versions_are_not_overwritten() {
        let directory = tempdir().unwrap();
        let store = LibraryStore::new(directory.path().join("library.json"));
        fs::write(store.path(), br#"{"version":999,"capsules":[]}"#).unwrap();

        assert!(matches!(
            store.load(),
            Err(StoreError::UnsupportedVersion { found: 999, .. })
        ));
        assert!(matches!(
            store.create(record(directory.path(), "No overwrite")),
            Err(StoreError::UnsupportedVersion { found: 999, .. })
        ));
    }
}

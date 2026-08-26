//! Read-only inspection of an existing `.capsule` image.
//!
//! Existing images are registered in place instead of being copied. Their
//! contents are never executed during import: Capsule takes the same exclusive
//! image lock used by the launcher, mounts the filesystem read-only, and scans
//! only the fixed portable-game subtree with the hardened folder inspector.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use rustix::fs::{FileType, Mode, OFlags, fstat, open};
use serde::Deserialize;
use uuid::Uuid;

use crate::backend::capabilities::CapabilityReport;
use crate::backend::portable::{
    ImportLimits, PORTABLE_GAME_ROOT, PortableImportError, PortableInspection,
    inspect_capsule_game_directory,
};
use crate::backend::storage::{ImageMountPlan, StorageError, validate_image_path};
use crate::backend::validate_host_absolute;

const MANIFEST_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ExistingCapsuleInspection {
    /// Canonical absolute path saved in the library.
    pub image_path: PathBuf,
    pub inspection: PortableInspection,
}

/// Validate and inspect a user-selected capsule without changing it.
pub fn inspect_existing_capsule(
    image_path: &Path,
    runtime_root: &Path,
    limits: &ImportLimits,
    capabilities: &CapabilityReport,
) -> Result<ExistingCapsuleInspection, ExistingCapsuleError> {
    validate_host_absolute(image_path)?;
    validate_host_absolute(runtime_root)?;
    validate_image_path(image_path)?;

    let metadata =
        fs::symlink_metadata(image_path).map_err(|source| io_error(image_path, source))?;
    if !metadata.file_type().is_file() {
        return Err(ExistingCapsuleError::NotRegular(image_path.to_path_buf()));
    }

    let image_path = fs::canonicalize(image_path).map_err(|source| io_error(image_path, source))?;
    validate_image_path(&image_path)?;
    let image = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image_path)
        .map_err(|source| io_error(&image_path, source))?;
    match image.try_lock_exclusive() {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
            return Err(ExistingCapsuleError::Busy(image_path));
        }
        Err(source) => return Err(io_error(&image_path, source)),
    }

    let result = inspect_locked_image(&image_path, runtime_root, limits, capabilities);
    FileExt::unlock(&image).ok();
    result.map(|inspection| ExistingCapsuleInspection {
        image_path,
        inspection,
    })
}

fn inspect_locked_image(
    image_path: &Path,
    runtime_root: &Path,
    limits: &ImportLimits,
    capabilities: &CapabilityReport,
) -> Result<PortableInspection, ExistingCapsuleError> {
    fs::create_dir_all(runtime_root).map_err(|source| io_error(runtime_root, source))?;
    fs::set_permissions(runtime_root, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(runtime_root, source))?;

    let run_dir = runtime_root.join(format!(
        "existing-inspect-{}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let mount_point = run_dir.join("root");
    fs::create_dir(&run_dir).map_err(|source| io_error(&run_dir, source))?;
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(&run_dir, source))?;

    let mount = match ImageMountPlan::new_read_only(image_path, &mount_point, capabilities) {
        Ok(mount) => mount,
        Err(error) => {
            let _ = fs::remove_dir(&run_dir);
            return Err(error.into());
        }
    };
    if let Err(error) = mount.execute_mount() {
        // Never recurse after a partial mount failure. Ordinary empty
        // directories can be detached safely; ambiguous state is preserved.
        let detached = match fs::remove_dir(&mount_point) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if detached {
            let _ = fs::remove_dir(&run_dir);
        }
        return Err(error.into());
    }

    let fallback_name = image_path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Existing capsule")
        .to_owned();
    let inspection_result = inspect_mounted_capsule(&mount_point, fallback_name, limits);
    let unmount_result = mount.execute_unmount();
    if unmount_result.is_ok() {
        let _ = fs::remove_dir_all(&run_dir);
    }

    match (inspection_result, unmount_result) {
        (Ok(inspection), Ok(())) => Ok(inspection),
        (Err(error), Ok(())) => Err(error),
        (_, Err(error)) => Err(error.into()),
    }
}

fn inspect_mounted_capsule(
    root: &Path,
    fallback_name: String,
    limits: &ImportLimits,
) -> Result<PortableInspection, ExistingCapsuleError> {
    let game = root.join("prefix").join(PORTABLE_GAME_ROOT);
    let metadata = fs::symlink_metadata(&game).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ExistingCapsuleError::UnsupportedLayout(game.clone())
        } else {
            io_error(&game, source)
        }
    })?;
    if !metadata.file_type().is_dir() {
        return Err(ExistingCapsuleError::UnsupportedLayout(game));
    }

    let name = read_manifest_name(root).unwrap_or(fallback_name);
    inspect_capsule_game_directory(&game, name, limits).map_err(Into::into)
}

fn read_manifest_name(root: &Path) -> Option<String> {
    let path = root.join(".capsule/manifest.json");
    let descriptor = open(
        &path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    let metadata = fstat(&descriptor).ok()?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || metadata.st_size < 0
        || metadata.st_size as u64 > MANIFEST_LIMIT
    {
        return None;
    }
    let mut file = File::from(descriptor);
    let mut bytes = Vec::with_capacity(metadata.st_size as usize);
    file.by_ref()
        .take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return None;
    }
    let manifest: ExistingManifest = serde_json::from_slice(&bytes).ok()?;
    if manifest.format_version != 1 {
        return None;
    }
    let name = manifest.name.trim();
    if name.is_empty() || name.len() > 512 || name.chars().any(|character| character.is_control()) {
        return None;
    }
    Some(name.to_owned())
}

#[derive(Deserialize)]
struct ExistingManifest {
    format_version: u32,
    name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ExistingCapsuleError {
    #[error(transparent)]
    InvalidPath(#[from] crate::backend::PathValidationError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Portable(#[from] PortableImportError),
    #[error("selected capsule is not a regular file: {0:?}")]
    NotRegular(PathBuf),
    #[error("capsule is already running or being inspected: {0:?}")]
    Busy(PathBuf),
    #[error("unsupported capsule layout; expected a portable game directory at {0:?}")]
    UnsupportedLayout(PathBuf),
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> ExistingCapsuleError {
    ExistingCapsuleError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn mounted_capsule_uses_manifest_and_returns_prefix_relative_launcher() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let game = root.join("prefix/drive_c/Game/bin");
        fs::create_dir_all(&game).unwrap();
        fs::write(game.join("game.exe"), b"MZ\0\0").unwrap();
        fs::create_dir(root.join(".capsule")).unwrap();
        fs::write(
            root.join(".capsule/manifest.json"),
            br#"{"format_version":1,"name":"Imported Game"}"#,
        )
        .unwrap();

        let inspection =
            inspect_mounted_capsule(root, "fallback".into(), &ImportLimits::default()).unwrap();
        assert_eq!(inspection.suggested_name, "Imported Game");
        assert_eq!(
            inspection.executable_candidates,
            vec![PathBuf::from("drive_c/Game/bin/game.exe")]
        );
    }

    #[test]
    fn mounted_capsule_rejects_a_linked_game_directory() {
        let temp = tempdir().unwrap();
        fs::create_dir_all(temp.path().join("prefix/drive_c")).unwrap();
        fs::create_dir(temp.path().join("outside")).unwrap();
        symlink(
            temp.path().join("outside"),
            temp.path().join("prefix/drive_c/Game"),
        )
        .unwrap();

        assert!(matches!(
            inspect_mounted_capsule(temp.path(), "fallback".into(), &ImportLimits::default()),
            Err(ExistingCapsuleError::UnsupportedLayout(_))
        ));
    }

    #[test]
    fn malformed_manifest_falls_back_to_file_name() {
        let temp = tempdir().unwrap();
        fs::create_dir_all(temp.path().join("prefix/drive_c/Game")).unwrap();
        fs::write(temp.path().join("prefix/drive_c/Game/game.exe"), b"MZ").unwrap();
        fs::create_dir(temp.path().join(".capsule")).unwrap();
        fs::write(temp.path().join(".capsule/manifest.json"), b"not json").unwrap();

        let inspection =
            inspect_mounted_capsule(temp.path(), "file name".into(), &ImportLimits::default())
                .unwrap();
        assert_eq!(inspection.suggested_name, "file name");
    }

    #[test]
    fn selected_path_must_be_a_capsule_file_before_mounting() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("game.img");
        fs::write(&path, b"not an image").unwrap();
        let capabilities = CapabilityReport::default();

        assert!(matches!(
            inspect_existing_capsule(&path, temp.path(), &ImportLimits::default(), &capabilities),
            Err(ExistingCapsuleError::Storage(
                StorageError::WrongImageExtension(_)
            ))
        ));
    }
}

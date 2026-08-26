//! Per-run image lifecycle supervision.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use fs2::FileExt;
use uuid::Uuid;

use crate::backend::capabilities::detect_with_environment_override;
use crate::backend::launcher::{
    LaunchError, build_launch_plan_with, build_launch_plan_with_status,
    build_wine_prepare_plan_with, build_wine_utility_launch_plan_with_status,
};
use crate::backend::steam::{SteamInstallerError, validate_installer};
use crate::backend::storage::{ImageMountPlan, StorageError};
use crate::model::{CapsuleRecord, NetworkPolicy, RunnerKind, StorageKind};
use crate::paths::{AppPaths, PathError, runtime_root};
use crate::store::{LibraryStore, StoreError};

pub fn run_from_library(id: Uuid) -> Result<ExitStatus, SupervisorError> {
    let paths = AppPaths::discover()?;
    let record = LibraryStore::new(paths.library_file).get(id)?;
    run(&record)
}

pub fn run(record: &CapsuleRecord) -> Result<ExitStatus, SupervisorError> {
    run_with_mode(record, false)
}

/// Copy Valve's installer into a Wine capsule and run it as an interactive
/// utility. The library entrypoint is never changed.
pub fn install_steam_from_library(
    id: Uuid,
    installer: &Path,
) -> Result<ExitStatus, SupervisorError> {
    validate_installer(installer)?;
    let paths = AppPaths::discover()?;
    let record = LibraryStore::new(paths.library_file).get(id)?;
    ensure_wine_record(&record)?;
    copy_steam_installer(&record, installer)?;

    let mut installer_record = steam_utility_record(&record);
    installer_record.entrypoint = PathBuf::from("drive_c/Capsule/Installers/SteamSetup.exe");
    installer_record.working_dir = Some(PathBuf::from("drive_c/Capsule/Installers"));
    run_with_mode(&installer_record, true)
}

/// Open the Windows Steam client already installed in this capsule. Waiting
/// for the complete Wine server keeps Steam's updater and login subprocesses
/// alive until the user explicitly closes the client.
pub fn open_steam_from_library(id: Uuid) -> Result<ExitStatus, SupervisorError> {
    let paths = AppPaths::discover()?;
    let record = LibraryStore::new(paths.library_file).get(id)?;
    ensure_wine_record(&record)?;

    let mut steam_record = steam_utility_record(&record);
    steam_record.entrypoint = PathBuf::from("drive_c/Program Files (x86)/Steam/steam.exe");
    steam_record.working_dir = Some(PathBuf::from("drive_c/Program Files (x86)/Steam"));
    run_with_mode(&steam_record, true)
}

fn run_with_mode(
    record: &CapsuleRecord,
    wine_utility: bool,
) -> Result<ExitStatus, SupervisorError> {
    let capabilities = detect_with_environment_override()?;
    match &record.storage {
        StorageKind::DirectoryDev { .. } => {
            let plan = if wine_utility {
                build_wine_utility_launch_plan_with_status(record, &capabilities, None)?
            } else {
                build_launch_plan_with(record, &capabilities)?
            };
            prepare_wine(record, &capabilities)?;
            emit_warnings(&plan.warnings);
            let status = plan.command.execute().map_err(SupervisorError::Spawn)?;
            ensure_success(status)
        }
        StorageKind::Image { path } | StorageKind::ExternalImage { path } => {
            let image_lock = lock_image(path)?;
            let runtime_root = runtime_root()?;
            fs::create_dir_all(&runtime_root).map_err(|source| io_error(&runtime_root, source))?;
            fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700))
                .map_err(|source| io_error(&runtime_root, source))?;
            let run_dir = runtime_root.join(format!(
                "run-{}-{}-{}",
                record.id.simple(),
                std::process::id(),
                Uuid::new_v4().simple()
            ));
            let mount_point = run_dir.join("root");
            fs::create_dir(&run_dir).map_err(|source| io_error(&run_dir, source))?;
            fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))
                .map_err(|source| io_error(&run_dir, source))?;

            // Wine engines can extract embedded DLLs into the prefix and map
            // them as code. Isolation is provided by the nested Bubblewrap
            // sandbox; import and metadata mounts remain `noexec`.
            let mount = ImageMountPlan::new_runnable(path, &mount_point, &capabilities)?;
            if let Err(error) = mount.execute_mount() {
                // Do not recurse through a mountpoint after a partial mount
                // failure. Empty ordinary directories can be removed safely;
                // anything else is preserved for explicit recovery.
                let _ = fs::remove_dir(&mount_point);
                let _ = fs::remove_dir(&run_dir);
                return Err(error.into());
            }

            let mut mounted_record = record.clone();
            let prefix = mount_point.join("prefix");
            mounted_record.storage = StorageKind::DirectoryDev { path: prefix };
            let contained_status = run_dir.join("contained-exit-status");
            let run_result = (|| {
                let plan = if wine_utility {
                    build_wine_utility_launch_plan_with_status(
                        &mounted_record,
                        &capabilities,
                        Some(&contained_status),
                    )?
                } else {
                    build_launch_plan_with_status(
                        &mounted_record,
                        &capabilities,
                        Some(&contained_status),
                    )?
                };
                prepare_wine(&mounted_record, &capabilities)?;
                emit_warnings(&plan.warnings);
                let status = plan.command.execute().map_err(SupervisorError::Spawn)?;
                ensure_trusted_child_success(status, &contained_status)
            })();
            let unmount_result = mount.execute_unmount();
            // A failed unmount means `root` may still refer to the live image.
            // Recursive cleanup in that state could delete capsule contents.
            if unmount_result.is_ok() {
                let _ = fs::remove_dir_all(&run_dir);
            }
            FileExt::unlock(&image_lock).ok();

            match (run_result, unmount_result) {
                (Ok(status), Ok(())) => Ok(status),
                (Err(error), Ok(())) => Err(error),
                (_, Err(error)) => Err(error.into()),
            }
        }
    }
}

fn steam_utility_record(record: &CapsuleRecord) -> CapsuleRecord {
    let mut utility = record.clone();
    utility.arguments.clear();
    utility.wine_virtual_desktop = None;
    utility.wine_steam = false;
    utility.permissions.gpu = true;
    utility.permissions.network = match utility.permissions.network {
        NetworkPolicy::Lan => NetworkPolicy::Lan,
        _ => NetworkPolicy::InternetOnly,
    };
    utility.permissions.controllers = false;
    utility
}

fn ensure_wine_record(record: &CapsuleRecord) -> Result<(), SupervisorError> {
    if record.runner == RunnerKind::Wine {
        Ok(())
    } else {
        Err(SupervisorError::SteamRequiresWine)
    }
}

fn copy_steam_installer(record: &CapsuleRecord, installer: &Path) -> Result<(), SupervisorError> {
    match &record.storage {
        StorageKind::DirectoryDev { path } => copy_installer_into_prefix(path, installer),
        StorageKind::Image { path } | StorageKind::ExternalImage { path } => {
            let image_lock = lock_image(path)?;
            let capabilities = detect_with_environment_override()?;
            let runtime_root = runtime_root()?;
            fs::create_dir_all(&runtime_root).map_err(|source| io_error(&runtime_root, source))?;
            fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700))
                .map_err(|source| io_error(&runtime_root, source))?;
            let copy_dir = runtime_root.join(format!(
                "steam-installer-copy-{}-{}-{}",
                record.id.simple(),
                std::process::id(),
                Uuid::new_v4().simple()
            ));
            let mount_point = copy_dir.join("root");
            fs::create_dir(&copy_dir).map_err(|source| io_error(&copy_dir, source))?;
            fs::set_permissions(&copy_dir, fs::Permissions::from_mode(0o700))
                .map_err(|source| io_error(&copy_dir, source))?;
            let mount = ImageMountPlan::new_runnable(path, &mount_point, &capabilities)?;
            if let Err(error) = mount.execute_mount() {
                let _ = fs::remove_dir(&mount_point);
                let _ = fs::remove_dir(&copy_dir);
                FileExt::unlock(&image_lock).ok();
                return Err(error.into());
            }

            let copy_result = copy_installer_into_prefix(&mount_point.join("prefix"), installer);
            let unmount_result = mount.execute_unmount();
            if unmount_result.is_ok() {
                let _ = fs::remove_dir_all(&copy_dir);
            }
            FileExt::unlock(&image_lock).ok();
            match (copy_result, unmount_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) => Err(error),
                (_, Err(error)) => Err(error.into()),
            }
        }
    }
}

fn copy_installer_into_prefix(root: &Path, installer: &Path) -> Result<(), SupervisorError> {
    let drive_c = root.join("drive_c");
    require_plain_directory(&drive_c)?;
    let capsule_dir = drive_c.join("Capsule");
    create_or_require_plain_directory(&capsule_dir)?;
    let installers_dir = capsule_dir.join("Installers");
    create_or_require_plain_directory(&installers_dir)?;
    let destination = installers_dir.join("SteamSetup.exe");
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(SupervisorError::UnsafeInstallerDestination(destination)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&destination, source)),
    }
    fs::copy(installer, &destination).map_err(|source| io_error(&destination, source))?;
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(&destination, source))?;
    Ok(())
}

fn create_or_require_plain_directory(path: &Path) -> Result<(), SupervisorError> {
    match fs::symlink_metadata(path) {
        Ok(_) => require_plain_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|source| io_error(path, source))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|source| io_error(path, source))
        }
        Err(source) => Err(io_error(path, source)),
    }
}

fn require_plain_directory(path: &Path) -> Result<(), SupervisorError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(SupervisorError::UnsafeInstallerDestination(
            path.to_path_buf(),
        ))
    }
}

fn prepare_wine(
    record: &CapsuleRecord,
    capabilities: &crate::backend::capabilities::CapabilityReport,
) -> Result<(), SupervisorError> {
    let Some(command) = build_wine_prepare_plan_with(record, capabilities)? else {
        return Ok(());
    };
    let status = command.execute().map_err(SupervisorError::Spawn)?;
    ensure_success(status)?;
    Ok(())
}

fn lock_image(path: &Path) -> Result<File, SupervisorError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(SupervisorError::UnsafeImage(path.to_path_buf()));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    file.try_lock_exclusive()
        .map_err(|source| SupervisorError::AlreadyRunning(path.to_path_buf(), source))?;
    Ok(file)
}

fn emit_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("Capsule warning: {warning}");
    }
}

fn ensure_success(status: ExitStatus) -> Result<ExitStatus, SupervisorError> {
    if status.success() {
        Ok(status)
    } else {
        Err(SupervisorError::ProcessFailed(status))
    }
}

fn ensure_trusted_child_success(
    outer_status: ExitStatus,
    status_path: &Path,
) -> Result<ExitStatus, SupervisorError> {
    match fs::read_to_string(status_path) {
        Ok(contents) => {
            let code = contents
                .trim()
                .parse::<i32>()
                .map_err(|_| SupervisorError::InvalidContainedStatus(contents))?;
            if code == 0 {
                Ok(outer_status)
            } else {
                Err(SupervisorError::ContainedProcessFailed(code))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => ensure_success(outer_status),
        Err(source) => Err(io_error(status_path, source)),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error(transparent)]
    Paths(#[from] PathError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Capability(#[from] crate::backend::capabilities::CapabilityError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    SteamInstaller(#[from] SteamInstallerError),
    #[error("Steam can be installed only in Wine capsules")]
    SteamRequiresWine,
    #[error("Steam installer destination is not a plain capsule file or directory: {0:?}")]
    UnsafeInstallerDestination(PathBuf),
    #[error("capsule image is not a regular non-symlink file: {0:?}")]
    UnsafeImage(PathBuf),
    #[error("capsule is already running or locked: {0:?}: {1}")]
    AlreadyRunning(PathBuf, #[source] io::Error),
    #[error("could not start sandbox supervisor command: {0}")]
    Spawn(#[source] io::Error),
    #[error("contained application exited unsuccessfully: {0}")]
    ProcessFailed(ExitStatus),
    #[error("contained application exited unsuccessfully with status {0}")]
    ContainedProcessFailed(i32),
    #[error("trusted contained-exit status was malformed: {0:?}")]
    InvalidContainedStatus(String),
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> SupervisorError {
    SupervisorError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn installer_copy_stays_below_plain_prefix_directories() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir_all(prefix.join("drive_c")).unwrap();
        let source = temp.path().join("SteamSetup.exe");
        fs::write(&source, b"installer").unwrap();

        copy_installer_into_prefix(&prefix, &source).unwrap();
        assert_eq!(
            fs::read(prefix.join("drive_c/Capsule/Installers/SteamSetup.exe")).unwrap(),
            b"installer"
        );
    }

    #[test]
    fn installer_copy_rejects_capsule_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let outside = temp.path().join("outside");
        fs::create_dir_all(prefix.join("drive_c")).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, prefix.join("drive_c/Capsule")).unwrap();
        let source = temp.path().join("SteamSetup.exe");
        fs::write(&source, b"installer").unwrap();

        assert!(matches!(
            copy_installer_into_prefix(&prefix, &source),
            Err(SupervisorError::UnsafeInstallerDestination(_))
        ));
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }
}

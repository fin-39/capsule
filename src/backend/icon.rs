//! Safe, disposable icon-cache generation for capsule entries.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use uuid::Uuid;

use crate::backend::capabilities::{Capability, CapabilityReport};
use crate::backend::storage::{ImageMountPlan, StorageError, resolve_inside_capsule};
use crate::backend::validate_host_absolute;
use crate::model::{CapsuleRecord, RunnerKind, StorageKind};

/// Extract a record's application icon into a disposable cache file.
///
/// The PE parser runs as a separate process inside Bubblewrap with only the
/// selected executable and a fresh output directory visible. Capsule images
/// are mounted read-only and locked against concurrent game launches.
pub fn cache_record_icon(
    record: &CapsuleRecord,
    destination: &Path,
    runtime_root: &Path,
    capsule_executable: &Path,
    capabilities: &CapabilityReport,
) -> Result<(), IconError> {
    validate_host_absolute(destination)?;
    validate_host_absolute(runtime_root)?;
    validate_host_absolute(capsule_executable)?;
    if destination.exists() {
        return Ok(());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| IconError::UnsafePath(destination.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    let icon_source = icon_source_entrypoint(record);

    match &record.storage {
        StorageKind::DirectoryDev { path } => {
            let executable = resolve_executable(path, &icon_source)?;
            extract_sandboxed(&executable, destination, capsule_executable, capabilities)
        }
        StorageKind::Image { path } | StorageKind::ExternalImage { path } => {
            let image_lock = lock_image(path)?;
            fs::create_dir_all(runtime_root).map_err(|source| io_error(runtime_root, source))?;
            fs::set_permissions(runtime_root, fs::Permissions::from_mode(0o700))
                .map_err(|source| io_error(runtime_root, source))?;
            let run_dir = runtime_root.join(format!(
                "icon-{}-{}-{}",
                record.id.simple(),
                std::process::id(),
                Uuid::new_v4().simple()
            ));
            let mount_point = run_dir.join("root");
            fs::create_dir(&run_dir).map_err(|source| io_error(&run_dir, source))?;
            fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))
                .map_err(|source| io_error(&run_dir, source))?;

            let mount = ImageMountPlan::new_read_only(path, &mount_point, capabilities)?;
            if let Err(error) = mount.execute_mount() {
                let _ = fs::remove_dir(&mount_point);
                let _ = fs::remove_dir(&run_dir);
                return Err(error.into());
            }
            let result = resolve_executable(&mount_point.join("prefix"), &icon_source).and_then(
                |executable| {
                    extract_sandboxed(&executable, destination, capsule_executable, capabilities)
                },
            );
            let unmount_result = mount.execute_unmount();
            if unmount_result.is_ok() {
                let _ = fs::remove_dir_all(&run_dir);
            }
            FileExt::unlock(&image_lock).ok();
            match (result, unmount_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) => Err(error),
                (_, Err(error)) => Err(error.into()),
            }
        }
    }
}

fn icon_source_entrypoint(record: &CapsuleRecord) -> PathBuf {
    if record.runner == RunnerKind::Native
        && record
            .entrypoint
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sh"))
    {
        record.entrypoint.with_extension("exe")
    } else {
        record.entrypoint.clone()
    }
}

fn resolve_executable(root: &Path, relative: &Path) -> Result<PathBuf, IconError> {
    let candidate = resolve_inside_capsule(root, relative)?;
    let canonical_root = fs::canonicalize(root).map_err(|source| io_error(root, source))?;
    let canonical = fs::canonicalize(&candidate).map_err(|source| io_error(&candidate, source))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(IconError::UnsafePath(candidate));
    }
    let metadata =
        fs::symlink_metadata(&canonical).map_err(|source| io_error(&canonical, source))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(IconError::UnsafePath(canonical));
    }
    Ok(canonical)
}

fn extract_sandboxed(
    executable: &Path,
    destination: &Path,
    capsule_executable: &Path,
    capabilities: &CapabilityReport,
) -> Result<(), IconError> {
    let bwrap = capabilities
        .get(Capability::Bubblewrap)
        .ok_or(IconError::MissingCapability(Capability::Bubblewrap))?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| IconError::UnsafePath(destination.to_path_buf()))?;
    let temporary = destination_parent.join(format!(".icon-{}", Uuid::new_v4().simple()));
    fs::create_dir(&temporary).map_err(|source| io_error(&temporary, source))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(&temporary, source))?;

    let mut command = std::process::Command::new(bwrap);
    command.args([
        "--unshare-all",
        "--unshare-user",
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
        "--disable-userns",
        "--clearenv",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--dir",
        "/etc",
        "--ro-bind-try",
        "/etc/ImageMagick-7",
        "/etc/ImageMagick-7",
    ]);
    if let Some(bundle_root) =
        std::env::var_os("CAPSULE_BUNDLE_ROOT").filter(|value| !value.is_empty())
    {
        let bundle_root = PathBuf::from(bundle_root);
        let bundle_lib = bundle_root.join("usr/lib");
        let bundle_magick = bundle_root.join("usr/bin/magick");
        let bundle_magick_config = bundle_root.join("usr/share/capsule/imagemagick");
        let bundle_magick_modules = bundle_root.join("usr/lib/capsule/imagemagick/coders");
        for path in [
            &bundle_lib,
            &bundle_magick,
            &bundle_magick_config,
            &bundle_magick_modules,
        ] {
            validate_host_absolute(path)?;
        }
        command
            .args([
                "--dir",
                "/runtime",
                "--dir",
                "/runtime/bin",
                "--dir",
                "/runtime/share",
                "--dir",
                "/runtime/share/capsule",
                "--dir",
                "/runtime/lib-capsule",
            ])
            .arg("--ro-bind")
            .arg(&bundle_lib)
            .arg("/runtime/lib")
            .arg("--ro-bind")
            .arg(&bundle_magick)
            .arg("/runtime/bin/magick")
            .arg("--ro-bind")
            .arg(&bundle_magick_config)
            .arg("/runtime/share/capsule/imagemagick")
            .arg("--ro-bind")
            .arg(&bundle_magick_modules)
            .arg("/runtime/lib-capsule/imagemagick-coders")
            .args(["--setenv", "LD_LIBRARY_PATH", "/runtime/lib"])
            .args(["--setenv", "CAPSULE_MAGICK", "/runtime/bin/magick"])
            .args([
                "--setenv",
                "MAGICK_CONFIGURE_PATH",
                "/runtime/share/capsule/imagemagick",
            ])
            .args([
                "--setenv",
                "MAGICK_CODER_MODULE_PATH",
                "/runtime/lib-capsule/imagemagick-coders",
            ]);
    }
    command
        .args([
            "--dir",
            "/app",
            "--dir",
            "/input",
            "--dir",
            "/output",
            "--ro-bind",
        ])
        .arg(capsule_executable)
        .arg("/app/capsule")
        .arg("--ro-bind")
        .arg(executable)
        .arg("/input/game.exe")
        .arg("--bind")
        .arg(&temporary)
        .arg("/output")
        .args([
            "--",
            "/app/capsule",
            "--extract-icon",
            "/input/game.exe",
            "/output/icon.png",
        ]);
    let status = command.status().map_err(|source| io_error(bwrap, source));
    let generated = temporary.join("icon.png");
    let result = match status {
        Ok(status) if status.success() => {
            fs::rename(&generated, destination).map_err(|source| io_error(destination, source))?;
            fs::set_permissions(destination, fs::Permissions::from_mode(0o600))
                .map_err(|source| io_error(destination, source))
        }
        Ok(status) => Err(IconError::ExtractorFailed(status)),
        Err(error) => Err(error),
    };
    let _ = fs::remove_file(&generated);
    let _ = fs::remove_dir(&temporary);
    result
}

fn lock_image(path: &Path) -> Result<File, IconError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(IconError::UnsafePath(path.to_path_buf()));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    file.try_lock_exclusive()
        .map_err(|source| IconError::AlreadyRunning(path.to_path_buf(), source))?;
    Ok(file)
}

#[derive(Debug, thiserror::Error)]
pub enum IconError {
    #[error(transparent)]
    InvalidPath(#[from] crate::backend::PathValidationError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("required backend capability is unavailable: {0:?}")]
    MissingCapability(Capability),
    #[error("unsafe icon source or destination path: {0:?}")]
    UnsafePath(PathBuf),
    #[error("capsule is currently running: {0:?}: {1}")]
    AlreadyRunning(PathBuf, #[source] io::Error),
    #[error("sandboxed icon extractor exited unsuccessfully: {0}")]
    ExtractorFailed(std::process::ExitStatus),
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> IconError {
    IconError::Io {
        path: path.to_path_buf(),
        source,
    }
}
